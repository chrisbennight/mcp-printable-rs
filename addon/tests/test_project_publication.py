"""Create-only native export publication closes the destination race window."""

from pathlib import Path
import os
import tempfile
import unittest
from unittest.mock import patch

from printable_bridge.workspace import SecureWorkspace, WorkspaceError


class PublicationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.workspace = SecureWorkspace(self.root, self.root / "runtime")
        self.addCleanup(self.workspace.close)
        self.request = self.workspace.validate("exports/model.zip", ".zip")

    def test_publication_creates_new_artifact_and_rejects_a_second_commit(self):
        with self.workspace.stage_output(self.request) as output:
            output.path.write_bytes(b"prepared bundle")
            output.commit_new()
            with self.assertRaisesRegex(WorkspaceError, "already committed"):
                output.commit_new()
        self.assertEqual((self.root / self.request.relative).read_bytes(), b"prepared bundle")

    def test_file_or_symlink_created_during_preparation_is_never_replaced(self):
        for symlink in (False, True):
            with self.subTest(symlink=symlink):
                target = self.root / self.request.relative
                with self.workspace.stage_output(self.request) as output:
                    self.assertFalse(output.destination_exists)
                    output.path.write_bytes(b"prepared bundle")
                    if symlink:
                        other = self.root / "other.zip"
                        other.write_bytes(b"other writer")
                        target.symlink_to(other)
                    else:
                        target.write_bytes(b"other writer")
                    with self.assertRaisesRegex(WorkspaceError, "already exists"):
                        output.commit_new()
                    self.assertFalse(output.committed)
                self.assertEqual(target.read_bytes(), b"other writer")
                self.assertEqual(target.is_symlink(), symlink)
                target.unlink()

    def test_preexisting_destination_is_not_replaced(self):
        target = self.root / self.request.relative
        target.parent.mkdir()
        target.write_bytes(b"existing")
        with self.workspace.stage_output(self.request) as output:
            output.path.write_bytes(b"new")
            with self.assertRaisesRegex(WorkspaceError, "already exists"):
                output.commit_new()
        self.assertEqual(target.read_bytes(), b"existing")

    def test_batch_conflict_retains_published_files_and_reports_partial_outcome(self):
        second_request = self.workspace.validate("exports/second.zip", ".zip")
        with self.workspace.stage_output(self.request) as first, \
                self.workspace.stage_output(second_request) as second:
            first.path.write_bytes(b"first prepared file")
            second.path.write_bytes(b"second prepared file")
            (self.root / second_request.relative).write_bytes(b"other writer")
            with self.assertRaisesRegex(WorkspaceError, "earlier outputs may remain"):
                self.workspace.commit_batch([first, second])
        self.assertEqual((self.root / self.request.relative).read_bytes(), b"first prepared file")
        self.assertEqual((self.root / second_request.relative).read_bytes(), b"other writer")

    def test_failed_batch_never_unlinks_a_concurrent_replacement(self):
        second_request = self.workspace.validate("exports/second.zip", ".zip")
        target = self.root / self.request.relative
        replacement = self.root / "replacement.zip"
        replacement.write_bytes(b"concurrent replacement")
        original_commit = self.workspace._commit_output
        original_stat = os.stat
        failed = False

        def fail_second(stage, parent, leaf, **kwargs):
            nonlocal failed
            if leaf == "second.zip":
                failed = True
                raise WorkspaceError("injected conflict")
            return original_commit(stage, parent, leaf, **kwargs)

        def replace_after_observation(path, *args, **kwargs):
            observed = original_stat(path, *args, **kwargs)
            if failed and path == "model.zip" and kwargs.get("dir_fd") is not None:
                os.replace(replacement, target)
            return observed

        with self.workspace.stage_output(self.request) as first, \
                self.workspace.stage_output(second_request) as second:
            first.path.write_bytes(b"first")
            second.path.write_bytes(b"second")
            with patch.object(self.workspace, "_commit_output", side_effect=fail_second), \
                    patch("os.stat", side_effect=replace_after_observation), \
                    self.assertRaises(WorkspaceError):
                self.workspace.commit_batch([first, second])
        # No rollback means no racy pathname observation or deletion at all.
        if replacement.exists():
            os.replace(replacement, target)
        self.assertEqual(target.read_bytes(), b"concurrent replacement")

    def test_post_publication_failure_retains_other_writer_and_reports_uncertainty(self):
        target = self.root / self.request.relative
        with self.workspace.stage_output(self.request) as output:
            output.path.write_bytes(b"prepared")
            original_link = os.link

            def replace_after_link(*args, **kwargs):
                original_link(*args, **kwargs)
                replacement = self.root / "replacement.zip"
                replacement.write_bytes(b"other writer")
                os.replace(replacement, target)

            with patch("os.link", side_effect=replace_after_link), \
                    self.assertRaisesRegex(WorkspaceError, "outcome is uncertain"):
                output.commit_new()
        self.assertEqual(target.read_bytes(), b"other writer")
