from __future__ import annotations

import os
from contextlib import nullcontext
from pathlib import Path
from types import SimpleNamespace
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

from printable_bridge.execution import (
    MAX_OUTPUT_BYTES,
    MAX_RESULT_BYTES,
    WATCHDOG_RESTART_EXIT_CODE,
    CodeExecutionError,
    execute_code,
)
from printable_bridge.watchdog import NoopExecutionWatchdog


class CodeExecutionTests(unittest.TestCase):
    def test_process_exit_during_status_read_does_not_reject_execution(self) -> None:
        real_open = open
        ghost = "4294967294"

        def process_open(path, *args, **kwargs):
            if path == f"/proc/{ghost}/status":
                raise ProcessLookupError()
            return real_open(path, *args, **kwargs)

        def process_entries(_path):
            return nullcontext([
                SimpleNamespace(name=str(os.getpid())),
                SimpleNamespace(name=ghost),
            ])

        with patch("os.scandir", side_effect=process_entries), patch(
            "builtins.open", side_effect=process_open
        ):
            response = self.execute("result = 1")
        self.assertEqual(response["result"], 1)

    def test_owned_display_sibling_survives_but_other_siblings_are_rejected(self) -> None:
        worker = """
import sys
from types import SimpleNamespace
from printable_bridge.execution import execute_code
from printable_bridge.watchdog import NoopExecutionWatchdog
execute_code('result = 1', 1.0, SimpleNamespace(), '/workspace', lambda: False,
             NoopExecutionWatchdog(), owned_display_pid=int(sys.argv[1]))
"""
        supervisor = """
import subprocess
import sys
children = []
try:
    display = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(10)'])
    children.append(display)
    if sys.argv[2] == 'extra':
        children.append(subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(10)']))
    completed = subprocess.run([sys.executable, '-c', sys.argv[1], str(display.pid)], timeout=3)
    assert display.poll() is None
    sys.exit(completed.returncode)
finally:
    for child in children:
        child.kill()
        child.wait()
"""
        for mode, expected in (("owned", 0), ("extra", WATCHDOG_RESTART_EXIT_CODE)):
            with self.subTest(mode=mode):
                completed = subprocess.run(
                    [sys.executable, "-c", supervisor, worker, mode],
                    capture_output=True, text=True, timeout=5,
                )
                self.assertEqual(completed.returncode, expected, completed.stderr)

    @staticmethod
    def execute_subprocess(
        source: str,
        timeout_seconds: float = 0.05,
        shutdown_requested: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        script = (
            "from types import SimpleNamespace\n"
            "from printable_bridge.execution import execute_code\n"
            "from printable_bridge.watchdog import NoopExecutionWatchdog\n"
            f"source = {source!r}\n"
            "execute_code(\n"
            f"    source, {timeout_seconds!r}, SimpleNamespace(), '/workspace', "
            f"lambda: {shutdown_requested!r}, NoopExecutionWatchdog(),\n"
            ")\n"
        )
        return subprocess.run(
            [sys.executable, "-c", script],
            capture_output=True,
            check=False,
            timeout=2,
            text=True,
        )

    def execute(
        self,
        source: str,
        timeout_seconds: float = 1.0,
        shutdown_requested=lambda: False,
    ) -> dict:
        return execute_code(
            source,
            timeout_seconds,
            SimpleNamespace(app=SimpleNamespace(version_string="5.2.0")),
            "/workspace",
            shutdown_requested,
            NoopExecutionWatchdog(),
        )

    def test_result_and_bounded_output_are_returned(self) -> None:
        response = self.execute(
            """
import sys
print("created")
print("warning", file=sys.stderr)
result = {"version": bpy.app.version_string, "workspace": workspace_root}
"""
        )

        self.assertEqual(
            response["result"],
            {"version": "5.2.0", "workspace": "/workspace"},
        )
        self.assertEqual(response["stdout"], "created\n")
        self.assertEqual(response["stderr"], "warning\n")
        self.assertFalse(response["stdout_truncated"])
        self.assertFalse(response["stderr_truncated"])
        self.assertGreaterEqual(response["elapsed_ms"], 0)

        keyed = self.execute('result = {2: "two", None: "none", 1.5: "float"}')
        self.assertEqual(
            keyed["result"], {"2": "two", "null": "none", "1.5": "float"}
        )

    def test_output_is_truncated_without_interrupting_execution(self) -> None:
        response = self.execute(
            f'print("é" * {MAX_OUTPUT_BYTES * 16}); result = "finished"'
        )

        self.assertEqual(response["result"], "finished")
        self.assertLessEqual(
            len(response["stdout"].encode("utf-8")), MAX_OUTPUT_BYTES
        )
        self.assertTrue(response["stdout_truncated"])

    def test_descriptor_and_subprocess_output_share_the_response_bound(self) -> None:
        response = self.execute(
            f"""
import os
import subprocess
import sys
os.write(1, b"native-out\\n")
os.write(2, b"native-error\\n")
subprocess.run(
    [sys.executable, "-c", "print('child-out')"],
    check=True,
)
os.write(1, b"x" * {MAX_OUTPUT_BYTES * 2})
result = "finished"
"""
        )

        self.assertEqual(response["result"], "finished")
        self.assertIn("native-out\n", response["stdout"])
        self.assertIn("child-out\n", response["stdout"])
        self.assertEqual(response["stderr"], "native-error\n")
        self.assertLessEqual(
            len(response["stdout"].encode("utf-8")), MAX_OUTPUT_BYTES
        )
        self.assertTrue(response["stdout_truncated"])

    def test_background_python_thread_forces_process_recovery(self) -> None:
        completed = self.execute_subprocess(
            "import threading\n"
            "import time\n"
            "threading.Thread(target=lambda: time.sleep(60), daemon=True).start()\n"
            "result = 'returned'\n",
            timeout_seconds=1.0,
        )

        self.assertEqual(completed.returncode, WATCHDOG_RESTART_EXIT_CODE)

    @unittest.skipUnless(sys.platform == "linux", "Linux process containment")
    def test_background_process_is_killed_and_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            pid_path = Path(temporary, "background.pid")
            completed = self.execute_subprocess(
                "import subprocess\n"
                "import sys\n"
                "from pathlib import Path\n"
                "child = subprocess.Popen(\n"
                "    [sys.executable, '-c', 'while True: pass'],\n"
                "    close_fds=True,\n"
                "    start_new_session=True,\n"
                ")\n"
                f"Path({str(pid_path)!r}).write_text(str(child.pid), encoding='utf-8')\n"
                "result = 'returned'\n",
                timeout_seconds=1.0,
            )

            self.assertNotEqual(completed.returncode, 0)
            self.assertIn("background processes", completed.stderr)
            pid = int(pid_path.read_text(encoding="utf-8"))
            deadline = time.monotonic() + 2.0
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                os.kill(pid, signal.SIGKILL)
                self.fail("background process survived synchronous execution")

    def test_source_and_timeout_are_validated_before_execution(self) -> None:
        cases = [
            ("", 1.0, "non-empty"),
            ("result = None", True, "positive finite"),
            ("result = None", 0.0, "positive finite"),
            ("result = None", -1.0, "positive finite"),
            ("result = None", float("nan"), "positive finite"),
        ]
        for source, timeout, message in cases:
            with self.subTest(timeout=timeout, message=message):
                with self.assertRaisesRegex(CodeExecutionError, message):
                    self.execute(source, timeout)

        self.assertEqual(self.execute("result = 3600", 3600.0)["result"], 3600)

    def test_compilation_is_inside_the_work_budget_and_elapsed_time(self) -> None:
        real_compile = compile
        watchdog = Mock()

        def delayed_compile(*args, **kwargs):
            self.assertTrue(watchdog.arm.called)
            time.sleep(0.03)
            return real_compile(*args, **kwargs)

        with patch("builtins.compile", side_effect=delayed_compile):
            response = execute_code(
                "result = 1",
                1.0,
                SimpleNamespace(),
                "/workspace",
                lambda: False,
                watchdog,
            )

        self.assertGreaterEqual(response["elapsed_ms"], 20)
        watchdog.disarm.assert_called_once_with()

    def test_syntax_and_runtime_errors_are_caller_visible_without_tracebacks(
        self,
    ) -> None:
        with self.assertRaisesRegex(
            CodeExecutionError, "syntax error on line 1"
        ):
            self.execute("if:")
        with self.assertRaisesRegex(
            CodeExecutionError, "code raised ValueError: bad input"
        ):
            self.execute('raise ValueError("bad input")')
        with self.assertRaisesRegex(CodeExecutionError, "code raised SystemExit"):
            self.execute("raise SystemExit(4)")
        for source, name in (
            ("raise BaseException('stop')", "BaseException"),
            (
                "class StopExecution(BaseException):\n"
                "    pass\n"
                "raise StopExecution()",
                "StopExecution",
            ),
        ):
            with self.subTest(name=name), self.assertRaisesRegex(
                CodeExecutionError, f"code raised {name}"
            ):
                self.execute(source)

    def test_runaway_python_is_interrupted_and_trace_state_is_restored(self) -> None:
        completed = self.execute_subprocess("while True:\n    pass")
        self.assertEqual(completed.returncode, WATCHDOG_RESTART_EXIT_CODE)

        previous_trace = sys.gettrace()
        response = self.execute("result = 7")
        self.assertIs(sys.gettrace(), previous_trace)
        self.assertEqual(response["result"], 7)

    def test_shutdown_interrupts_running_code(self) -> None:
        completed = self.execute_subprocess(
            "while True:\n    pass", shutdown_requested=True
        )
        self.assertEqual(completed.returncode, 0)

    def test_caller_code_cannot_escape_the_real_watchdog(self) -> None:
        cases = {
            "broad exception handler": (
                "try:\n"
                "    while True:\n"
                "        pass\n"
                "except BaseException:\n"
                "    while True:\n"
                "        pass\n"
            ),
            "exception formatting": (
                "class BlockingError(Exception):\n"
                "    def __str__(self):\n"
                "        while True:\n"
                "            pass\n"
                "raise BlockingError()\n"
            ),
            "result serialization": (
                "class BlockingResult(dict):\n"
                "    def items(self):\n"
                "        while True:\n"
                "            pass\n"
                "result = BlockingResult(value=1)\n"
            ),
        }
        for name, caller_source in cases.items():
            with self.subTest(name=name):
                completed = self.execute_subprocess(caller_source)

                self.assertEqual(
                    completed.returncode, WATCHDOG_RESTART_EXIT_CODE
                )

    def test_result_must_be_bounded_finite_json(self) -> None:
        boundary_characters = (MAX_RESULT_BYTES - 2) // 2
        boundary = self.execute(
            f'result = "\\n" * {boundary_characters}'
        )
        self.assertEqual(len(boundary["result"]), boundary_characters)

        cases = [
            (
                f'result = "x" * {MAX_RESULT_BYTES * 2}',
                f"{MAX_RESULT_BYTES}-byte limit",
            ),
            (
                f'result = "\\n" * {boundary_characters + 1}',
                f"{MAX_RESULT_BYTES}-byte limit",
            ),
            ('result = float("nan")', "finite JSON data"),
            ("result = object()", "finite JSON data"),
        ]
        for source, message in cases:
            with self.subTest(message=message):
                with self.assertRaisesRegex(CodeExecutionError, message):
                    self.execute(source)


if __name__ == "__main__":
    unittest.main()
