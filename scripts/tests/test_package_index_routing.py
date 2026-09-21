"""Publication routes Rust dependencies through the approved crate proxy.

Cargo reads no environment variable for a mirror, so the redirect is a config
file rather than a setting the caller can simply export. A site that never gets
one fails silently: cargo resolves from crates.io and the build goes green, so
nothing in a log distinguishes a working redirect from an absent one.
"""

from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from textwrap import dedent


ROOT = Path(__file__).resolve().parents[2]
BUILD_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
TEST_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
DOCKERFILE = ROOT / "Dockerfile"
BUILD_SCRIPT = ROOT / "build-docker.sh"

class PackageIndexRoutingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.build = BUILD_WORKFLOW.read_text()
        cls.test = TEST_WORKFLOW.read_text()
        cls.dockerfile = DOCKERFILE.read_text()
        cls.script = BUILD_SCRIPT.read_text()

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
            # `test -x` on the exported smoke driver has to find something.
            (root / "workdir").mkdir()
            base = {
                key: value
                for key, value in os.environ.items()
                if key != "CRATES_INDEX_URL"
            }
            result = subprocess.run(
                ["bash", str(BUILD_SCRIPT), *arguments],
                env=base | {"PATH": f"{root}:{os.environ['PATH']}", "DOCKER_LOG": str(log)} | environment,
                capture_output=True,
                text=True,
                check=False,
                cwd=str(root),
            )
            return result, log.read_text().splitlines() if log.exists() else []

    def test_the_argument_carries_no_default(self) -> None:
        """`ARG NAME=` would hand cargo an empty registry to replace with.

        Declared bare, an unsupplied build leaves it genuinely unset, no config
        file is written, and cargo resolves from crates.io - the fallback that
        keeps this image buildable away from the proxy's network.
        """
        self.assertIn("\nARG CRATES_INDEX_URL\n", self.dockerfile)
        self.assertNotIn("ARG CRATES_INDEX_URL=", self.dockerfile)

    def test_the_redirect_is_written_after_the_context_is_copied(self) -> None:
        """A config file written before `COPY . .` would be overwritten."""
        copy_at = self.dockerfile.index("COPY . .")
        config_at = self.dockerfile.index(">> .cargo/config.toml")
        build_at = self.dockerfile.index("cargo build --release --locked --bin printable-server")

        self.assertLess(copy_at, config_at)
        self.assertLess(config_at, build_at)

    def test_the_redirect_keeps_the_lockfile_publicly_resolvable(self) -> None:
        """Source replacement, not a second registry.

        Replacing the existing crates.io source leaves `Cargo.lock` naming
        `crates-io`, so a lock produced behind the proxy still resolves from the
        public index. An added registry would rewrite those entries.
        """
        for name, source in (("Dockerfile", self.dockerfile),):
            with self.subTest(source=name):
                self.assertIn('[source.crates-io]', source)
                self.assertIn('replace-with = "mirror"', source)
        self.assertNotIn("nexus", (ROOT / "Cargo.lock").read_text())

    def test_the_name_stays_outside_cargos_own_namespace(self) -> None:
        """`CARGO_REGISTRY_INDEX` aborts every cargo invocation.

        Cargo maps it onto its removed `registry.index` key, and a Dockerfile
        `ARG` reaches `RUN` as an environment variable - so that spelling would
        break the build it was meant to route. Comments explaining this are
        exempt; what must not appear is a use of the name.
        """
        for name, source in (
            ("Dockerfile", self.dockerfile),
            ("release.yml", self.build),
            ("ci.yml", self.test),
            ("build-docker.sh", self.script),
        ):
            with self.subTest(source=name):
                effective = "\n".join(
                    line for line in source.splitlines() if not line.lstrip().startswith("#")
                )
                self.assertNotIn("CARGO_REGISTRY_INDEX", effective)


    def test_the_publishing_script_routes_its_own_cargo_commands(self) -> None:
        """`--push` resolves dependencies on the host as well as in an image.

        Passed as cargo's own `--config` assignments rather than by writing
        `.cargo/config.toml`, because this path refuses to publish from a dirty
        worktree and a generated file would be exactly that.
        """
        self.assertIn('source.crates-io.replace-with="mirror"', self.script)
        for command in ("clippy", "test"):
            with self.subTest(command=command):
                self.assertIn(f'cargo "${{cargo_index_args[@]}}" {command}', self.script)


    def test_publishing_refuses_to_fall_back_to_the_public_index(self) -> None:
        """A pull request may fall back; a run that pushes may not.

        An absent address on a publishing run means the fleet's injection
        regressed, and the pushed image would carry crates that bypassed the
        proxy's cache, audit, and blocklist.
        """
        self.assertIn('CRATES_INDEX_URL: ${{ vars.CRATES_INDEX_URL }}', self.build)
        self.assertIn('test -n "${CRATES_INDEX_URL}"', self.build)
        self.assertIn('./build-docker.sh --push', self.build)

        # The local script refuses before it builds anything, so the caller
        # learns immediately rather than after a full release build.
        refused, refused_calls = self.run_build_script({}, arguments=("--push",))
        self.assertEqual(refused.returncode, 1)
        self.assertIn("refusing to publish", refused.stderr)
        self.assertEqual(refused_calls, [])

    def test_the_local_script_forwards_at_every_site_that_builds_rust(self) -> None:
        """Three of its five builds compile Rust; the other two must not change.

        The Blender image installs a hash-pinned wheel by direct URL and the
        release-pair image is `FROM scratch`; neither resolves from a crate
        index, so forwarding there would be noise.
        """
        rust_sites = [
            line
            for line in self.script.splitlines()
            if '"${index_build_args[@]}"' in line
        ]

        self.assertEqual(len(rust_sites), 3)
        self.assertNotIn('"${index_build_args[@]}"',
                         self.script[self.script.index("--file blender/Dockerfile"):])


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
        self.assertEqual(configured.returncode, bare.returncode)


if __name__ == "__main__":
    unittest.main()
