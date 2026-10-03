"""Public builds use crates.io; configured mirrors reach every Rust build."""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from textwrap import dedent


ROOT = Path(__file__).resolve().parents[2]
BUILD_SCRIPT = ROOT / "build-docker.sh"

class PackageIndexRoutingTests(unittest.TestCase):
    def run_build_script(self, environment, arguments=()):
        """Drive the local build script against a recording docker stub."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "docker.log"
            for name in ("docker", "git", "curl"):
                stub = root / name
                stub.write_text(
                    dedent(
                        f"""\
                        #!/bin/sh
                        if [ "{name}" = docker ]; then
                          printf '%s\\n' "$*" >> "$DOCKER_LOG"
                          exit 73
                        fi
                        if [ "{name}" = git ]; then
                          case "$1" in
                            "rev-parse") echo 000000000000 ;;
                            "status") : ;;
                          esac
                        fi
                        """
                    )
                )
                stub.chmod(0o755)
            shutil.copyfile(BUILD_SCRIPT, root / "build-docker.sh")
            (root / "scripts").mkdir()
            shutil.copyfile(ROOT / "scripts/release_identity.py", root / "scripts/release_identity.py")
            # Exercise a checkout with an existing export, as in a release run.
            # The Docker stub stops before any build or nested test execution.
            (root / "target/release").mkdir(parents=True)
            smoke = root / "target/release/printable-smoke"
            smoke.write_text("#!/bin/sh\nexit 99\n")
            smoke.chmod(0o755)
            base = {
                key: value
                for key, value in os.environ.items()
                if key != "CRATES_INDEX_URL"
            }
            result = subprocess.run(
                ["bash", str(root / "build-docker.sh"), *arguments],
                env=base | {"PATH": f"{root}:{os.environ['PATH']}", "DOCKER_LOG": str(log)} | environment,
                capture_output=True,
                text=True,
                check=False,
                cwd=str(root),
            )
            return result, log.read_text().splitlines() if log.exists() else []

    def test_retired_local_publication_flags_fail_before_building(self):
        for mode in ("--push", "--push-candidate"):
            with self.subTest(mode=mode):
                result, calls = self.run_build_script({}, arguments=(mode,))
                self.assertEqual(result.returncode, 2)
                self.assertEqual(calls, [])

    def test_a_local_build_forwards_by_value_and_omits_an_unset_name(self) -> None:
        """The stub records what actually reached the daemon.

        Forwarded by value because the daemon never sees the caller's
        environment, and omitted entirely when unset - an empty `--build-arg`
        would pin a source naming an empty registry.
        """
        configured, configured_calls = self.run_build_script(
            {"CRATES_INDEX_URL": "sparse+https://index.example/"}
        )
        bare, bare_calls = self.run_build_script({})

        for calls in (configured_calls, bare_calls):
            self.assertTrue(calls, "the script issued no docker command")

        self.assertIn(
            "--build-arg CRATES_INDEX_URL=sparse+https://index.example/",
            "\n".join(configured_calls),
        )
        self.assertNotIn("CRATES_INDEX_URL", "\n".join(bare_calls))
        # Both paths reach a build; neither aborts before issuing one.
        self.assertEqual(configured.returncode, 73)
        self.assertEqual(bare.returncode, 73)


if __name__ == "__main__":
    unittest.main()
