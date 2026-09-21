"""The disposable native child is bounded and never inherits service secrets."""

import json
from pathlib import Path
import signal
import tempfile
import unittest
from unittest.mock import patch

from printable_bridge.project_packing import ProjectPackingError
from printable_bridge.project_process import ProjectPreparationCancelled, prepare_in_child


class Child:
    pid = 1234

    def __init__(self):
        self.returncode = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        return False

    def poll(self):
        return self.returncode

    def wait(self, timeout=None):
        self.returncode = 0
        return 0


class ProcessTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)

    def invoke(self, **options):
        return prepare_in_child("/opt/blender/blender", self.root / "original", self.root / "staged",
                                ["model.blend"], "model.blend", options.pop("timeout", 1), **options)

    def test_command_uses_fixed_script_data_arguments_and_allowlisted_environment(self):
        child = Child()
        observed = {}

        def launch(command, **kwargs):
            observed.update(kwargs)
            observed["command"] = command
            request = json.loads(Path(command[-2]).read_text())
            self.assertEqual(request["entrypoint"], "model.blend")
            Path(command[-1]).write_text(json.dumps({"entrypoint": "model.blend",
                                                   "registered_external_files": 0}))
            return child

        with patch("printable_bridge.project_process.subprocess.Popen", side_effect=launch):
            result = self.invoke()
        self.assertEqual(result["registered_external_files"], 0)
        self.assertEqual(set(observed["env"]), {"PATH", "TMPDIR", "LANG"})
        self.assertTrue(observed["start_new_session"])
        self.assertIn("--disable-autoexec", observed["command"])
        self.assertFalse(Path(observed["cwd"]).exists())
        self.assertNotIn("shell", observed)

    def test_invalid_deadline_and_early_cancellation_never_launch(self):
        with patch("printable_bridge.project_process.subprocess.Popen") as launch:
            for timeout in (True, 0, 121, float("inf"), float("nan")):
                with self.subTest(timeout=timeout), self.assertRaises(ProjectPackingError):
                    self.invoke(timeout=timeout)
            with self.assertRaises(ProjectPreparationCancelled):
                self.invoke(cancelled=lambda: True)
            launch.assert_not_called()

    def test_deadline_or_cancellation_kills_and_reaps_child(self):
        module = "printable_bridge.project_process"
        for cancelled in (False, True):
            child = Child()
            checks = iter((False, cancelled))
            with self.subTest(cancelled=cancelled), \
                    patch(f"{module}.subprocess.Popen", return_value=child), \
                    patch(f"{module}.os.killpg") as kill, \
                    patch(f"{module}.time.monotonic", side_effect=(0, 2)):
                with self.assertRaisesRegex(ProjectPackingError, "cancelled|deadline"):
                    self.invoke(cancelled=lambda: next(checks))
                kill.assert_called_once_with(child.pid, signal.SIGKILL)
                self.assertEqual(child.returncode, 0)

    def test_failed_child_or_missing_metadata_cannot_claim_completion(self):
        module = "printable_bridge.project_process"
        for code in (1, 0):
            child = Child()
            child.returncode = code
            with self.subTest(code=code), patch(f"{module}.subprocess.Popen", return_value=child):
                with self.assertRaisesRegex(ProjectPackingError, "failed|no bounded metadata"):
                    self.invoke()
