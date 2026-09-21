"""Reject changes that move untrusted CI across the release trust boundary."""

from pathlib import Path
import shutil
import tempfile
import unittest

import yaml

from workflow_policy import read_workflow, validate


ROOT = Path(__file__).resolve().parents[2]


class WorkflowPolicyTests(unittest.TestCase):
    def test_current_workflows(self):
        self.assertEqual(validate(ROOT), [])

    def test_unsafe_workflow_mutations(self):
        cases = [
            ("release.yml", lambda w: w["on"].update({"pull_request": {}})),
            ("release.yml", lambda w: w["jobs"]["release"].update({"if": "true"})),
            ("release.yml", lambda w: w["jobs"]["release"].update({"if": "github.event_name == 'workflow_dispatch' && vars.PRINTABLE_RELEASE_ENABLED == 'true'"})),
            ("release.yml", lambda w: w["jobs"]["release"].update({"if": "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main'"})),
            ("release.yml", lambda w: w["jobs"]["release"].update({"environment": "unprotected"})),
            ("release.yml", lambda w: w["jobs"]["release"]["env"].update({"CRATES_INDEX_URL": ""})),
            ("release.yml", lambda w: w["jobs"]["release"]["steps"][0].update({"uses": "actions/checkout@main"})),
            ("ci.yml", lambda w: w["jobs"]["verify"].update({"runs-on": "self-hosted"})),
            ("ci.yml", lambda w: w["jobs"]["verify"].update({"permissions": {"packages": "write"}})),
            ("ci.yml", lambda w: w["jobs"]["verify"].update({"env": {"TOKEN": "${{ secrets.GITHUB_TOKEN }}"}})),
        ]
        for filename, mutation in cases:
            with self.subTest(filename=filename, mutation=cases.index((filename, mutation))):
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    shutil.copytree(ROOT / ".github", root / ".github")
                    shutil.copyfile(ROOT / "build-docker.sh", root / "build-docker.sh")
                    path = root / ".github/workflows" / filename
                    workflow = read_workflow(path)
                    mutation(workflow)
                    path.write_text(yaml.safe_dump(workflow))
                    self.assertTrue(validate(root))
