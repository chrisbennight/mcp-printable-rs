"""Bundled instruction changes must validate and rebuild the compiled server."""

from pathlib import Path
import os
import subprocess
import tempfile
import unittest


SCOPE = Path(__file__).resolve().parents[1] / "ci-scope.sh"


class BundledSkillScopeTests(unittest.TestCase):
    def test_skill_edits_select_rust_images_and_publication(self):
        for event in ("pull_request", "push"):
            for name in ("image-to-mold", "inspect-printer-camera"):
                with self.subTest(event=event, name=name), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)

                    def git(*args):
                        return subprocess.run(
                            ["git", "-c", "user.name=Scope Test", "-c", "user.email=scope@example.invalid",
                             "-c", "commit.gpgsign=false", *args],
                            cwd=root, check=True, capture_output=True, text=True,
                        ).stdout.strip()

                    git("init", "-b", "main")
                    skill = root / "skills" / name / "SKILL.md"
                    skill.parent.mkdir(parents=True)
                    skill.write_text("Original instructions\n")
                    git("add", "skills")
                    git("commit", "-m", "Base")
                    base = git("rev-parse", "HEAD")
                    skill.write_text("Revised instructions\n")
                    git("add", "skills")
                    git("commit", "-m", "Revise bundled skill")
                    output = root / "scope-output"
                    subprocess.run(
                        ["bash", str(SCOPE)], cwd=root, check=True,
                        env={**os.environ, "GITHUB_EVENT_NAME": event,
                             "BASE_SHA": base, "TARGET_SHA": git("rev-parse", "HEAD"),
                             "GITHUB_OUTPUT": str(output)},
                    )
                    scope = dict(line.split("=", 1) for line in output.read_text().splitlines())
                    self.assertEqual(scope, {
                        "rust": "true", "addon": "false", "tooling": "false",
                        "docs": "true", "workflows": "false", "fuzz": "false",
                        "images": "true", "shell": "false", "publish": "true",
                    })

    def scope_for_changed_input(self, name):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args):
                return subprocess.run(
                    ["git", "-c", "user.name=Scope Test", "-c", "user.email=scope@example.invalid",
                     "-c", "commit.gpgsign=false", *args],
                    cwd=root, check=True, capture_output=True, text=True,
                ).stdout.strip()

            git("init", "-b", "main")
            worker = root / name
            worker.parent.mkdir(parents=True, exist_ok=True)
            worker.write_text("original = True\n")
            git("add", name)
            git("commit", "-m", "Base")
            base = git("rev-parse", "HEAD")
            worker.write_text("original = False\n")
            git("add", name)
            git("commit", "-m", "Revise the worker")
            output = root / "scope-output"
            subprocess.run(
                ["bash", str(SCOPE)], cwd=root, check=True,
                env={**os.environ, "GITHUB_EVENT_NAME": "push",
                     "BASE_SHA": base, "TARGET_SHA": git("rev-parse", "HEAD"),
                     "GITHUB_OUTPUT": str(output)},
            )
            scope = dict(line.split("=", 1) for line in output.read_text().splitlines())
            return scope

    def test_reconstruction_worker_edits_select_images_and_publication(self):
        scope = self.scope_for_changed_input("recon/reconstruct.py")
        self.assertEqual(scope["images"], "true")
        self.assertEqual(scope["publish"], "true")

    def test_image_set_definition_edits_select_images_and_publication(self):
        scope = self.scope_for_changed_input("docker-bake.hcl")
        self.assertEqual(scope["images"], "true")
        self.assertEqual(scope["publish"], "true")

    def test_scanner_only_edits_do_not_publish_without_tested_images(self):
        scope = self.scope_for_changed_input("scripts/release_security.py")
        self.assertEqual(scope["tooling"], "true")
        self.assertEqual(scope["images"], "false")
        self.assertEqual(scope["publish"], "false")


if __name__ == "__main__":
    unittest.main()
