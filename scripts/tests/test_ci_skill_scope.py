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


if __name__ == "__main__":
    unittest.main()
