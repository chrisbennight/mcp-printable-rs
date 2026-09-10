from __future__ import annotations

import os
from contextlib import nullcontext
from pathlib import Path
from types import SimpleNamespace
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

from printable_bridge.display import DisplayError, PrivateDisplay
from printable_bridge.supervisor import BlenderSupervisor, _direct_child_pids
from printable_bridge.config import ConfigError, blender_ui_backend


class PrivateDisplayTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.display = PrivateDisplay(Path(temporary.name))
        self.addCleanup(self.display.close, 0)
        self.process = Mock(pid=4242)
        self.process.poll.return_value = None

    def start(self) -> tuple[Mock, Mock]:
        def ready(*_args: object, **kwargs: object) -> Mock:
            os.write(kwargs["pass_fds"][0], b"99\n")
            return self.process

        with patch("printable_bridge.display.subprocess.run") as xauth, patch(
            "printable_bridge.display.subprocess.Popen", side_effect=ready
        ) as xvfb:
            self.display.start()
        return xauth, xvfb

    def test_ready_display_has_private_authentication_and_no_tcp_listener(self) -> None:
        xauth, xvfb = self.start()
        environment = self.display.environment
        authority = Path(environment["XAUTHORITY"])
        self.assertEqual(environment["DISPLAY"], ":99")
        self.assertEqual(stat.S_IMODE(authority.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(authority.parent.stat().st_mode), 0o700)
        self.assertNotIn("MIT-MAGIC-COOKIE-1", " ".join(xauth.call_args.args[0]))
        self.assertIs(xauth.call_args.kwargs["stderr"], subprocess.DEVNULL)
        command = xvfb.call_args.args[0]
        self.assertEqual(command[command.index("-nolisten") + 1], "tcp")
        self.assertEqual(command[command.index("-auth") + 1], str(authority))
        self.assertNotIn("-ac", command)

    def test_shutdown_reaps_display_and_removes_authority(self) -> None:
        self.start()
        authority = Path(self.display.environment["XAUTHORITY"])
        self.display.close(2)
        self.process.terminate.assert_called_once_with()
        self.process.wait.assert_called_once_with(timeout=2)
        self.assertFalse(authority.parent.exists())
        self.assertIsNone(self.display.pid)

    def test_shutdown_kills_display_when_grace_expires(self) -> None:
        self.start()
        self.process.wait.side_effect = [subprocess.TimeoutExpired("Xvfb", 1), 0]
        self.display.close(1)
        self.process.kill.assert_called_once_with()

    def test_missing_readiness_cleans_up_before_propagating_failure(self) -> None:
        with patch("printable_bridge.display.subprocess.run"), patch(
            "printable_bridge.display.subprocess.Popen", return_value=self.process
        ), self.assertRaises(DisplayError):
            self.display.start()
        self.process.terminate.assert_called_once_with()
        self.assertIsNone(self.display.pid)

    def test_dead_display_cannot_supply_environment(self) -> None:
        self.start()
        self.process.poll.return_value = 1
        with self.assertRaises(DisplayError):
            _ = self.display.environment


class DisplaySupervisorTests(unittest.TestCase):
    def test_process_exit_during_status_read_does_not_stop_supervision(self) -> None:
        entries = [SimpleNamespace(name="20", path="/proc/20"),
                   SimpleNamespace(name="30", path="/proc/30")]
        with patch("printable_bridge.supervisor.os.scandir",
                   return_value=nullcontext(entries)), patch.object(
            Path, "read_text", side_effect=[ProcessLookupError(), "PPid:\t10\n"]
        ):
            self.assertEqual(_direct_child_pids(10), [30])

    def test_process_scan_permission_failure_is_not_hidden(self) -> None:
        entries = [SimpleNamespace(name="20", path="/proc/20")]
        with patch("printable_bridge.supervisor.os.scandir",
                   return_value=nullcontext(entries)), patch.object(
            Path, "read_text", side_effect=PermissionError()
        ), self.assertRaises(PermissionError):
            _direct_child_pids(10)

    def test_caller_cleanup_excludes_the_owned_display(self) -> None:
        display = Mock(pid=200)
        supervisor = BlenderSupervisor(["blender"], Path("/unused"), 1, display=display)
        with patch(
            "printable_bridge.supervisor._direct_child_pids", side_effect=[[200, 300], [200]]
        ), patch("printable_bridge.supervisor.os.kill") as kill, patch(
            "printable_bridge.supervisor.os.waitpid"
        ) as wait:
            supervisor._kill_adopted_children()
        self.assertEqual([args.args[0] for args in kill.call_args_list], [300])
        self.assertEqual([args.args[0] for args in wait.call_args_list], [300])

    def test_reaping_excludes_blender_and_the_display(self) -> None:
        supervisor = BlenderSupervisor(["blender"], Path("/unused"), 1, display=Mock(pid=200))
        supervisor._child = Mock(pid=100)
        with patch(
            "printable_bridge.supervisor._direct_child_pids", return_value=[100, 200, 300]
        ), patch("printable_bridge.supervisor.os.waitpid") as wait:
            supervisor._reap_adopted_children()
        self.assertEqual([args.args[0] for args in wait.call_args_list], [300])

    def test_display_is_closed_after_the_blender_loop_returns(self) -> None:
        display = Mock(pid=200)
        supervisor = BlenderSupervisor(["blender"], Path("/unused"), 1, display=display)
        def finish() -> int:
            display.start.assert_called_once_with()
            display.close.assert_not_called()
            return 0
        with patch("printable_bridge.supervisor._enable_child_subreaper"), patch(
            "printable_bridge.supervisor.signal.signal"
        ), patch.object(supervisor, "_run_blender", side_effect=finish):
            self.assertEqual(supervisor.run(), 0)
        display.close.assert_called_once_with(1)

    def test_backend_names_are_validated(self) -> None:
        self.assertEqual(blender_ui_backend({}), "software")
        self.assertEqual(blender_ui_backend({"PRINTABLE_BLENDER_UI_BACKEND": "egl"}), "egl")
        with self.assertRaises(ConfigError):
            blender_ui_backend({"PRINTABLE_BLENDER_UI_BACKEND": "unknown"})


if __name__ == "__main__":
    unittest.main()
