"""Validate GPU selection before any Docker operation."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "smoke-blender-gpu"
IMAGE = "ghcr.io/chrisbennight/mcp-printable-blender:sha-" + "a" * 12 + "@sha256:" + "b" * 64


class GpuSelectionTests(unittest.TestCase):
    def run_fixture(self, device, query_result="2"):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            docker = root / "docker"
            docker.write_text('#!/bin/sh\nprintf invoked > "$FIXTURE_ROOT/docker-called"\nexit 1\n')
            docker.chmod(0o755)
            smi = root / "nvidia-smi"
            smi.write_text('''#!/bin/sh
printf '%s\n' "$*" >> "$FIXTURE_ROOT/smi-arguments"
case "$*" in
  *--query-gpu=index*) printf '%s\n' "$QUERY_RESULT" ;;
esac
''')
            smi.chmod(0o755)
            result = subprocess.run(["bash", str(SCRIPT), IMAGE], check=False,
                                    capture_output=True, text=True, env=dict(os.environ,
                                        PATH=str(root) + ":" + os.environ["PATH"],
                                        FIXTURE_ROOT=str(root), QUERY_RESULT=query_result,
                                        PRINTABLE_GPU_DEVICE=device))
            commands = root / "smi-arguments"
            return result, commands.read_text() if commands.exists() else "", (root / "docker-called").exists()

    def test_invalid_device_is_rejected_before_external_commands(self):
        for device in ("all", "0,1", "--help", "$(id)"):
            with self.subTest(device=device):
                result, commands, docker_called = self.run_fixture(device)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(commands, "")
                self.assertFalse(docker_called)

    def test_resolved_device_is_used_for_required_coexistence(self):
        result, commands, docker_called = self.run_fixture("GPU-abcd-1234")
        self.assertEqual(result.returncode, 1)
        self.assertIn("--id=GPU-abcd-1234 --query-gpu=index", commands)
        self.assertIn("--id=2 --query-compute-apps=pid", commands)
        self.assertIn("no pre-existing GPU workload", result.stderr)
        self.assertFalse(docker_called)

    def test_ambiguous_device_query_is_rejected(self):
        result, commands, docker_called = self.run_fixture("0", "0\n1")
        self.assertEqual(result.returncode, 1)
        self.assertNotIn("--query-compute-apps", commands)
        self.assertFalse(docker_called)
