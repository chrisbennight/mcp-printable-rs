"""Reject changes that move untrusted CI across the release trust boundary."""

from pathlib import Path
import shutil
import tempfile
import unittest

import yaml

from workflow_policy import read_workflow, validate


ROOT = Path(__file__).resolve().parents[2]


class WorkflowPolicyTests(unittest.TestCase):
    def test_native_worker_release_gates_cannot_be_removed(self):
        markers = (
            'smoke "$verified_server" linux/amd64 "$smoke_port"',
            'python3 scripts/verify_release_image.py cad "$verified_cad" "$revision"',
            'python3 scripts/verify_release_image.py slicer "$verified_slicer" "$revision"',
            'python3 scripts/release_security.py "$verified_server" "$verified_blender" "$verified_cad" "$verified_slicer"',
            '  /opt/printable/slicer-smoke.py',
            'python3 scripts/test-image-notices.py "$verified_server" "$verified_blender"',
            'python3 scripts/test-installation.py "$verified_server" "$verified_blender"',
            '  /opt/printable/cad/smoke.py --worker',
        )
        for marker in markers:
            with self.subTest(marker=marker), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                shutil.copytree(ROOT / ".github", root / ".github")
                script = (ROOT / "build-docker.sh").read_text()
                self.assertEqual(script.count(marker), 1)
                (root / "build-docker.sh").write_text(script.replace(marker, ""))
                self.assertTrue(validate(root))

    def test_current_workflows(self):
        self.assertEqual(validate(ROOT), [])

    def test_unsafe_workflow_mutations(self):
        cases = [
            ("release.yml", lambda w: w["on"].update({"pull_request": {}})),
            ("release.yml", lambda w: w["jobs"]["build"].update({"if": "true"})),
            ("release.yml", lambda w: w["jobs"]["build"].update({"if": "github.event_name == 'workflow_dispatch' && vars.PRINTABLE_RELEASE_ENABLED == 'true'"})),
            ("release.yml", lambda w: w["jobs"]["build"].update({"if": "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main'"})),
            ("publish-release.yml", lambda w: w["jobs"]["publish"]["steps"][-1].update({"run": "echo approved"})),
            ("publish-release.yml", lambda w: w["jobs"]["publish"]["env"].update({"QUALIFIER_ID": "1"})),
            ("release.yml", lambda w: w["jobs"]["build"].update({"runs-on": "self-hosted"})),
            ("publish-release.yml", lambda w: w["jobs"]["publish"].update({"runs-on": "self-hosted"})),
            ("release.yml", lambda w: w["jobs"]["build"]["env"].update({"CRATES_INDEX_URL": ""})),
            ("release.yml", lambda w: w["jobs"]["build"]["steps"][0].update({"uses": "actions/checkout@main"})),
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
