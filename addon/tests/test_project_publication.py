"""Create-only native export publication closes the destination race window."""

from pathlib import Path
import tempfile
import unittest

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

    def test_batch_race_preserves_other_writer_and_rolls_back_its_own_first_file(self):
        second_request = self.workspace.validate("exports/second.zip", ".zip")
        with self.workspace.stage_output(self.request) as first, \
                self.workspace.stage_output(second_request) as second:
            first.path.write_bytes(b"first prepared file")
            second.path.write_bytes(b"second prepared file")
            (self.root / second_request.relative).write_bytes(b"other writer")
            with self.assertRaisesRegex(WorkspaceError, "already exists"):
                self.workspace.commit_batch([first, second])
        self.assertFalse((self.root / self.request.relative).exists())
        self.assertEqual((self.root / second_request.relative).read_bytes(), b"other writer")
