"""Only the tested images may be published; failures never publish a release record."""
import unittest
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
        with patch("publish_images.tested_images", return_value=self.images), \
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
            self.assertEqual(publish_images("a" * 40, self.images), "release")
            self.assertTrue(all(event[0] == "scan" for event in events[:4]))
            self.assertEqual([event[2] for event in events if event[:2] == ["docker", "tag"]], list(self.images.values()))
            record.assert_called_once()
            update.assert_called_once()

    def test_update_tag_only_moves_for_current_main(self):
        for head, moves in (("a" * 40, True), ("b" * 40, False)):
            with self.subTest(head=head), \
                    patch("publish_images.subprocess.check_output", return_value=head + "\trefs/heads/main\n"), \
                    patch("publish_images.subprocess.run") as run:
                publish_update_tag("a" * 40, "record@sha256:" + "f" * 64, ReleaseIdentity())
                self.assertEqual(run.call_count, 2 if moves else 0)

    def test_scan_failure_prevents_all_pushes(self):
        with patch("publish_images.tested_images", return_value=self.images), \
                patch("publish_images.load_policy", return_value={"scanner_version": "0.110.0", "fail_on_kev": False}), \
                patch("publish_images.verify_grype_version"), \
                patch("publish_images.scan", return_value={}), \
                patch("publish_images.evaluate_report", return_value=ScanEvaluation((), ())), \
                patch("publish_images.emit_evaluation", return_value=False), \
                patch("publish_images.subprocess.run") as run, \
                patch("publish_images.publish") as record:
            with self.assertRaises(ValueError):
                publish_images("a" * 40, self.images)
            run.assert_not_called()
            record.assert_not_called()
