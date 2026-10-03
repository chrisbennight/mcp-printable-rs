"""Only the tested images may be published; failures never publish a release record."""
import unittest
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

from publish_images import publish_images, tested_images, publish_update_tag
from release_identity import ReleaseIdentity
from release_record import ROLES
from release_security import ScanEvaluation


class PublishImagesTests(unittest.TestCase):
    def setUp(self):
        self.images = {role: "sha256:" + str(index) * 64 for index, role in enumerate(ROLES)}

    def test_invalid_revision_has_no_external_effect(self):
        with patch("publish_images.inspect_image") as inspect:
            with self.assertRaises(ValueError):
                tested_images("main; echo unexpected")
            inspect.assert_not_called()

    def test_changed_transfer_fails_before_scanning_or_pushing(self):
        with patch("publish_images.tested_images", return_value=self.images), \
                patch("publish_images.subprocess.run") as run, \
                patch("publish_images.load_policy") as policy:
            with self.assertRaises(ValueError):
                publish_images("a" * 40, {})
            run.assert_not_called()
            policy.assert_not_called()

    def test_success_scans_all_images_then_pushes_exact_ids(self):
        events = []
        def run(argv, **kwargs):
            events.append(argv)
        def inspect(reference):
            role = next(role for role in ROLES if ("mcp-printable-" + role) in reference) if "mcp-printable-rs" not in reference else "server"
            return {"Id": self.images[role]}, []
        with TemporaryDirectory() as directory, \
                patch("publish_images.tested_images", return_value=self.images), \
                patch("publish_images.load_policy", return_value={"scanner_version": "0.110.0", "fail_on_kev": False}), \
                patch("publish_images.verify_grype_version"), \
                patch("publish_images.scan", side_effect=lambda image: events.append(["scan", image]) or {}), \
                patch("publish_images.evaluate_report", return_value=ScanEvaluation((), ())), \
                patch("publish_images.emit_evaluation", return_value=True), \
                patch("publish_images.subprocess.run", side_effect=run), \
                patch("publish_images.subprocess.check_output", return_value="sha256:" + "f" * 64), \
                patch("publish_images.inspect_image", side_effect=inspect), \
                patch("publish_images.verify", return_value=[]), \
                patch("publish_images.publish", return_value="release") as record, \
                patch("publish_images.publish_update_tag") as update:
            self.assertEqual(publish_images("a" * 40, self.images, Path(directory)), "release")
            for role, image in self.images.items():
                evidence = json.loads((Path(directory) / (role + ".json")).read_text())
                self.assertEqual(evidence["image_id"], image)
                self.assertEqual(evidence["revision"], "a" * 40)
                self.assertEqual(evidence["role"], role)
            self.assertTrue(all(event[0] == "scan" for event in events[:4]))
            self.assertEqual([event[2] for event in events if event[:2] == ["docker", "tag"]], list(self.images.values()))
            record.assert_called_once()
            update.assert_called_once()

    def test_update_tag_moves_only_when_current_application_inputs_match(self):
        import shutil
        import subprocess
        native_run = subprocess.run
        selector = Path(__file__).resolve().parents[1] / "ci-scope.sh"
        for changed, moves in (("README.md", True), ("addon/tests/test_fixture.py", True),
                               ("crates/printable-core/src/lib.rs", False)):
            with self.subTest(changed=changed), TemporaryDirectory() as directory:
                root = Path(directory) / "checkout"
                origin = Path(directory) / "origin.git"
                root.mkdir()
                (root / "scripts").mkdir()
                shutil.copyfile(selector, root / "scripts/ci-scope.sh")
                def git(*arguments):
                    return subprocess.check_output(
                        ["git", "-c", "user.name=CI validation", "-c", "user.email=ci@example.invalid", *arguments],
                        cwd=root, text=True, stderr=subprocess.DEVNULL).strip()
                git("init", "-q", "-b", "main")
                git("add", ".")
                git("commit", "-qm", "qualified source")
                revision = git("rev-parse", "HEAD")
                git("init", "--bare", "-q", str(origin))
                git("remote", "add", "origin", str(origin))
                path = root / changed
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("new main input")
                git("add", ".")
                git("commit", "-qm", "new main input")
                git("push", "-q", "origin", "main")
                git("checkout", "-q", revision)
                docker_calls = []
                def run(arguments, **kwargs):
                    if arguments[0] == "docker":
                        docker_calls.append(arguments)
                        return subprocess.CompletedProcess(arguments, 0)
                    return native_run(arguments, **kwargs)
                with patch("publish_images.ROOT", root), patch("publish_images.subprocess.run", side_effect=run):
                    publish_update_tag(revision, "record@sha256:" + "f" * 64, ReleaseIdentity())
                self.assertEqual([call[1] for call in docker_calls], ["tag", "push"] if moves else [])


    def test_scan_failure_prevents_all_pushes(self):
        with TemporaryDirectory() as directory, \
                patch("publish_images.tested_images", return_value=self.images), \
                patch("publish_images.load_policy", return_value={"scanner_version": "0.110.0", "fail_on_kev": False}), \
                patch("publish_images.verify_grype_version"), \
                patch("publish_images.scan", return_value={}) as scan, \
                patch("publish_images.evaluate_report", return_value=ScanEvaluation((), ())), \
                patch("publish_images.emit_evaluation", return_value=False), \
                patch("publish_images.subprocess.run") as run, \
                patch("publish_images.publish") as record:
            with self.assertRaises(ValueError):
                publish_images("a" * 40, self.images, Path(directory))
            run.assert_not_called()
            record.assert_not_called()
            self.assertEqual(scan.call_count, len(self.images))
            for role, image in self.images.items():
                evidence = json.loads((Path(directory) / (role + ".json")).read_text())
                self.assertEqual(evidence["image_id"], image)
                self.assertEqual(evidence["report"], {})
