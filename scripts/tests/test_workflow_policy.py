"""Publication follows successful hosted tests and is unavailable to PRs."""
from pathlib import Path
import shutil
import tempfile
import unittest
import yaml
from workflow_policy import read_workflow, validate

ROOT = Path(__file__).resolve().parents[2]

class WorkflowPolicyTests(unittest.TestCase):
    def test_current_workflow(self):
        self.assertEqual(validate(ROOT), [])

    def test_reject_unsafe_workflow_changes(self):
        changes = [
            lambda w: w["jobs"]["publish"].update({"if": "true"}),
            lambda w: w["jobs"]["publish"].update({"needs": ["verify"]}),
            lambda w: w["jobs"]["publish"].update({"permissions": {"contents": "write", "packages": "write"}}),
            lambda w: w["jobs"]["publish"].update({"runs-on": "self-hosted"}),
            lambda w: w["jobs"]["publish"].update({"environment": "approval"}),
            lambda w: w["jobs"]["verify"].update({"permissions": {"packages": "write"}}),
            lambda w: w["jobs"]["verify"].update({"env": {"TOKEN": "${{ secrets.GITHUB_TOKEN }}"}}),
            lambda w: w["jobs"]["publish"]["steps"][0].update({"uses": "actions/checkout@main"}),
            lambda w: w["jobs"]["publish"]["steps"][0]["with"].update({"persist-credentials": "true"}),
            lambda w: w["jobs"]["containers"].update({"steps": []}),
            lambda w: w["jobs"]["publish"]["steps"].append({"run": "docker build ."}),
            lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
        ]
        for index, change in enumerate(changes):
            with self.subTest(index=index), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                shutil.copytree(ROOT / ".github", root / ".github")
                path = root / ".github/workflows/ci.yml"
                workflow = read_workflow(path)
                change(workflow)
                path.write_text(yaml.safe_dump(workflow))
                self.assertTrue(validate(root))
