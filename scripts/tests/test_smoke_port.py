"""Local image smoke uses one validated loopback port throughout."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class SmokePortTests(unittest.TestCase):
    def test_port_reaches_container_health_check_and_mcp_client(self):
        for configured, expected in ((None, "8000"), ("58173", "58173"), ("0", None), ("65536", None), ("1+2", None)):
            with self.subTest(port=configured), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "scripts").mkdir()
                (root / "bin").mkdir()
                (root / "target/release").mkdir(parents=True)
                shutil.copyfile(ROOT / "build-docker.sh", root / "build-docker.sh")
                shutil.copyfile(ROOT / "scripts/release_identity.py", root / "scripts/release_identity.py")
                for name in ("docker", "git", "curl", "printable-smoke"):
                    target = root / ("target/release" if name == "printable-smoke" else "bin") / name
                    target.write_text(
                        "#!/bin/sh\n"
                        f"printf '{name} %s\\n' \"$*\" >> \"$TEST_LOG\"\n"
                        + ("[ \"$1\" != inspect ]\n" if name == "docker" else "exit 0\n")
                    )
                    target.chmod(0o755)
                environment = {key: value for key, value in os.environ.items() if key != "PRINTABLE_SMOKE_PORT"}
                environment.update(PATH=f"{root / 'bin'}:{os.environ['PATH']}", TEST_LOG=str(root / "calls.log"))
                if configured is not None:
                    environment["PRINTABLE_SMOKE_PORT"] = configured
                result = subprocess.run(
                    ["bash", str(root / "build-docker.sh")], env=environment,
                    capture_output=True, text=True, check=False, timeout=10,
                )
                log = root / "calls.log"
                if expected is None:
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("PRINTABLE_SMOKE_PORT", result.stderr)
                    self.assertFalse(log.exists())
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    calls = log.read_text().splitlines()
                    container = next(line for line in calls if line.startswith("docker run "))
                    self.assertIn(f"-p 127.0.0.1:{expected}:8123", container)
                    self.assertIn(f"curl -fsS http://127.0.0.1:{expected}/healthz", calls)
                    self.assertIn(f"printable-smoke http://127.0.0.1:{expected} smoke/expected-tools.txt", calls)


if __name__ == "__main__":
    unittest.main()
