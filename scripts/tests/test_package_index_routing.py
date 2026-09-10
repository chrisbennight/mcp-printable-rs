"""Every build site that compiles Rust must be redirectable to a crate mirror.

Cargo reads no environment variable for a mirror, so the redirect is a config
file rather than a setting the caller can simply export. A site that never gets
one fails silently: cargo resolves from crates.io and the build goes green, so
nothing in a log distinguishes a working redirect from an absent one.
"""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path
from textwrap import dedent


ROOT = Path(__file__).resolve().parents[2]
BUILD_WORKFLOW = ROOT / ".gitea" / "workflows" / "build.yml"
TEST_WORKFLOW = ROOT / ".gitea" / "workflows" / "test.yml"
DOCKERFILE = ROOT / "Dockerfile"
BUILD_SCRIPT = ROOT / "build-docker.sh"

PROXY_STEP_NAME = "Point cargo at the crate proxy"

# A cargo command that resolves dependencies on the runner itself. Anchored to
# the start of a command so prose mentioning cargo does not count, and `fmt` is
# excluded deliberately: it reads no registry, so it needs no redirect.
RUNNER_CARGO = re.compile(
    r"(?:^|[|;&]\s*|run:\s*)cargo(?:\s+\S*\+\S+)?\s+"
    r"(?:build|test|clippy|doc|run|check|fetch|install|fuzz)\b"
)


def runner_cargo_lines(body: str) -> list[str]:
    """Command lines in a job that make cargo resolve dependencies."""
    return [
        line
        for line in body.splitlines()
        if not line.strip().startswith("#")
        and "docker" not in line
        and RUNNER_CARGO.search(line.strip())
    ]


def jobs(workflow: str) -> dict[str, str]:
    """Split a workflow into its jobs, keyed by name.

    Parsed by indentation rather than with a YAML library: the runner's Python
    carries no third-party packages, and this file has to run there.
    """
    body = workflow[workflow.index("\njobs:\n") + len("\njobs:\n"):]
    starts = [
        (match.start(), match.group(1))
        for match in re.finditer(r"^  ([A-Za-z][\w-]*):$", body, re.MULTILINE)
    ]
    bounds = [s for s, _ in starts] + [len(body)]
    return {name: body[bounds[i]:bounds[i + 1]] for i, (_, name) in enumerate(starts)}



def step(source: str, name: str) -> str:
    marker = f"      - name: {name}\n"
    start = source.index(marker)
    following = source.find("\n      - ", start + len(marker))
    end = len(source) if following == -1 else following
    return dedent(source[start:end])


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
        for name, source in (
            ("Dockerfile", self.dockerfile),
            ("test.yml", step(self.test, PROXY_STEP_NAME)),
            ("build.yml", step(self.build, PROXY_STEP_NAME)),
        ):
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
            ("build.yml", self.build),
            ("test.yml", self.test),
            ("build-docker.sh", self.script),
        ):
            with self.subTest(source=name):
                effective = "\n".join(
                    line for line in source.splitlines() if not line.lstrip().startswith("#")
                )
                self.assertNotIn("CARGO_REGISTRY_INDEX", effective)

    def test_every_job_that_runs_cargo_is_redirected_before_it_resolves(self) -> None:
        """Per job, not per workflow: a config file cannot cross a job boundary.

        Each job gets its own runner and its own checkout, so a redirect written
        in one is invisible to the next. Checking only the first cargo command
        in a file would pass a workflow whose second job resolves from
        crates.io - which is exactly how the fuzz job was missed.
        """
        for name, source in (("test.yml", self.test), ("build.yml", self.build)):
            for job, body in jobs(source).items():
                cargo_steps = runner_cargo_lines(body)
                if not cargo_steps:
                    continue
                with self.subTest(workflow=name, job=job):
                    lines = body.splitlines()
                    proxy = next(
                        (i for i, line in enumerate(lines) if PROXY_STEP_NAME in line),
                        None,
                    )
                    self.assertIsNotNone(
                        proxy, f"{job} runs cargo with no proxy step: {cargo_steps}"
                    )
                    first_cargo = lines.index(cargo_steps[0])
                    self.assertLess(proxy, first_cargo)
                    # Container jobs run these scripts under sh, where -o
                    # pipefail aborts the step before it writes anything.
                    self.assertIn("shell: bash", body[body.index(PROXY_STEP_NAME):])

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

    def test_every_rust_compiling_build_site_forwards_the_address(self) -> None:
        """Three sites build the Rust Dockerfile; all three have to forward.

        The published image, the smoke driver exported from the same builder,
        and the local build. The Blender and release-pair images compile no Rust
        and are deliberately absent.
        """
        smoke_driver = step(self.build, "Build the smoke driver")
        publish = step(self.build, "Build and optionally publish")

        self.assertIn('index_build_args=(--build-arg "CRATES_INDEX_URL=${CRATES_INDEX_URL}")', smoke_driver)
        self.assertIn('"${index_build_args[@]}"', smoke_driver)
        # The action takes no shell, and an empty value is harmless here: the
        # Dockerfile writes nothing unless the argument is non-empty.
        self.assertIn("CRATES_INDEX_URL=${{ env.CRATES_INDEX_URL }}", publish)

    def test_publishing_refuses_to_fall_back_to_the_public_index(self) -> None:
        """A pull request may fall back; a run that pushes may not.

        An absent address on a publishing run means the fleet's injection
        regressed, and the pushed image would carry crates that bypassed the
        proxy's cache, audit, and blocklist.
        """
        gate = step(self.build, "Require the crate proxy when publishing")

        self.assertIn("steps.gate.outputs.push == 'true'", gate)
        self.assertIn("refusing to publish", gate)

        # The requirement reads the process environment while the build
        # argument is filled from the expression context. They agree on this
        # runner, but a check that only required the literal text would pass
        # just as happily if they ever stopped agreeing - so the workflow
        # compares them, and this requires that comparison to exist ahead of
        # the build and to be fatal.
        agreement = step(self.build, "Confirm the address reaches the build argument")
        self.assertIn("FROM_EXPRESSION: ${{ env.CRATES_INDEX_URL }}", agreement)
        self.assertIn('"${FROM_EXPRESSION:-}" != "${CRATES_INDEX_URL:-}"', agreement)
        self.assertIn("exit 1", agreement)
        self.assertLess(
            self.build.index("      - name: Confirm the address reaches the build argument"),
            self.build.index("      - name: Build and optionally publish\n"),
        )

        # The local script refuses before it builds anything, so the caller
        # learns immediately rather than after a full release build.
        refused, refused_calls = self.run_build_script({}, arguments=("--push",))
        self.assertEqual(refused.returncode, 1)
        self.assertIn("refusing to publish", refused.stderr)
        self.assertEqual(refused_calls, [])
        # The refusal has to precede the build it guards. Matched on the step
        # marker, since the Blender step's name shares this prefix.
        self.assertLess(
            self.build.index("      - name: Require the crate proxy when publishing"),
            self.build.index("      - name: Build and optionally publish\n"),
        )

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

    def test_the_probe_distinguishes_a_redirect_from_no_redirect(self) -> None:
        """A failing build proves nothing unless it failed at the index."""
        probe = step(self.build, "Check the crate index argument is load-bearing")

        self.assertIn("127.0.0.1:9", probe)
        self.assertIn('grep -q "127.0.0.1:9"', probe)
        self.assertIn("--target build", probe)
        self.assertIn("the build resolved crates without consulting", probe)
        # Bounded so an unreachable index cannot stall the job; the bound must
        # not become the pass condition, so the endpoint check stays required.
        self.assertIn("timeout 120 docker buildx build", probe)

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
